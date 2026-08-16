# syntax=docker/dockerfile:1

# Stage 1: Frontend Builder
FROM node:20-slim AS frontend-builder
WORKDIR /app
ARG ENABLE_MIRROR=false

# Configure NPM mirror if enabled
RUN if [ "$ENABLE_MIRROR" = "true" ]; then \
        npm config set registry https://registry.npmmirror.com ; \
    fi

# Use npm ci for deterministic builds, needs package-lock.json
COPY vite-project/package*.json ./vite-project/
RUN --mount=type=cache,target=/root/.npm \
    cd vite-project && npm ci
COPY vite-project/ ./vite-project/
RUN cd vite-project && npm run build

# Stage 2: Rust Builder
FROM rust:1.90-bookworm AS rust-builder
WORKDIR /app
ARG ENABLE_MIRROR=false

# Configure APT mirror if enabled (Debian Bookworm)
RUN if [ "$ENABLE_MIRROR" = "true" ]; then \
        sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list.d/debian.sources 2>/dev/null || \
        sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list ; \
    fi

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libasound2-dev \
    libpipewire-0.3-dev \
    libx11-dev \
    libxcb1-dev \
    libxcb-randr0-dev \
    libxext-dev \
    libssl-dev \
    clang \
    libclang-dev \
    cmake \
    nasm \
    libx264-dev \
    libvpx-dev \
    && rm -rf /var/lib/apt/lists/*

# use mirror
RUN mkdir -p /usr/local/cargo && \
    if [ "$ENABLE_MIRROR" = "true" ]; then \
        echo '[source.crates-io]\nreplace-with = "aliyun"\n\n[source.aliyun]\nregistry = "sparse+https://mirrors.aliyun.com/crates.io-index/"' > /usr/local/cargo/config.toml ; \
    fi

# Copy the entire project context. .dockerignore handles exclusions like node_modules and target/
COPY . .

# Copy frontend build results to server/static
COPY --from=frontend-builder /app/vite-project/dist ./server/static

# Build the main server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --bin lcxl-remote-desk-server && \
    cp target/release/lcxl-remote-desk-server .

# Stage 3: Runtime
FROM debian:bookworm-slim AS runtime
WORKDIR /app
ARG ENABLE_MIRROR=false

# Configure APT mirror if enabled
RUN if [ "$ENABLE_MIRROR" = "true" ]; then \
        sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list.d/debian.sources 2>/dev/null || \
        sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list ; \
    fi

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libasound2 \
    libpipewire-0.3-0 \
    libx11-6 \
    libxcb1 \
    libxcb-randr0 \
    libxext6 \
    libvpx7 \
    libx264-164 \
    ca-certificates \
    openssl \
    && rm -rf /var/lib/apt/lists/*

# Copy binary and static files
COPY --from=rust-builder /app/lcxl-remote-desk-server ./
COPY --from=rust-builder /app/server/static ./static

# Runtime state lives under the platform-standard paths the server resolves for
# the Linux system scope (it runs as root here), not under the working
# directory. Pre-creating them keeps the bind-mount targets explicit.
RUN mkdir -p /etc/lcxl-remote-desk /var/lib/lcxl-remote-desk /var/log/lcxl-remote-desk

# Set environment variables
ENV RUST_LOG=info

# Expose ports (default port is 8081)
EXPOSE 8081

# Start the server
CMD ["./lcxl-remote-desk-server", "--startup-mode", "signaling"]
