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
    --mount=type=cache,target=/app/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build --locked --release -p papermake-server && \
    cp ./target/release/papermake-server /bin/server

FROM ubuntu:24.04 AS fonts

# Exclude Ubuntu-C.ttf from the runtime font directory: Typst can treat it as
# an Ubuntu family face and resolve normal body text to UbuntuCondensed-Regular.
RUN sed -i -E '/^Components:/ { / universe/! s/$/ universe/; }' /etc/apt/sources.list.d/ubuntu.sources && \
    apt-get update && apt-get install -y --no-install-recommends \
    fonts-ubuntu-classic \
    && mkdir -p /opt/papermake/fonts/ubuntu \
    && cp \
    /usr/share/fonts/truetype/ubuntu/Ubuntu-B*.ttf \
    /usr/share/fonts/truetype/ubuntu/Ubuntu-L*.ttf \
    /usr/share/fonts/truetype/ubuntu/Ubuntu-M*.ttf \
    /usr/share/fonts/truetype/ubuntu/Ubuntu-R*.ttf \
    /usr/share/fonts/truetype/ubuntu/Ubuntu-Th.ttf \
    /usr/share/fonts/truetype/ubuntu/UbuntuMono-*.ttf \
    /opt/papermake/fonts/ubuntu/ \
    && rm -rf /var/lib/apt/lists/*

# Distroless runtime: ships glibc, libssl3, ca-certificates, and a nonroot
# user (uid 65532) — no shell, no package manager, nothing else. The `cc`
# variant adds libgcc/libstdc++ needed by the dynamically-linked Rust binary.
FROM gcr.io/distroless/cc-debian13:nonroot AS final

# Copy stable runtime assets before the application binary so app changes only
# invalidate the final server-binary layer.
COPY --from=fonts /opt/papermake/fonts/ubuntu /opt/papermake/fonts/ubuntu
ENV FONTS_DIR=/opt/papermake/fonts/ubuntu
CMD ["/app/server"]

# Copy the executable from the "build" stage last; this is the layer that
# changes when application code changes.
COPY --from=build /bin/server /app/server
