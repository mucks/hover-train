# Multi-stage build for hover-truck

# Stage 1: Build server
FROM rust:1.90-slim AS server-builder

WORKDIR /app

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace files (need all members for workspace to be valid)
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared
COPY server ./server
COPY client ./client

# Build server in release mode (limit parallelism to reduce memory usage)
ENV CARGO_BUILD_JOBS=2
RUN cargo build --release -p server

# Stage 2: Build WASM client
FROM rust:1.90-slim AS client-builder

WORKDIR /app

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install Trunk and wasm-bindgen-cli separately to reduce memory pressure
# Use --locked and limit parallelism to reduce memory usage
ENV CARGO_BUILD_JOBS=1
RUN cargo install --locked wasm-bindgen-cli && \
    cargo install --locked trunk

# Copy workspace files (need all members for workspace to be valid)
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared
COPY client ./client
COPY server ./server

# Build client WASM
WORKDIR /app/client
RUN trunk build --release

# Stage 3: Runtime image
FROM debian:bookworm-slim

WORKDIR /app

# Install nginx and wget (for healthcheck)
RUN apt-get update && apt-get install -y \
    nginx \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Copy server binary
COPY --from=server-builder /app/target/release/server /app/server

# Copy client static files
COPY --from=client-builder /app/client/dist /usr/share/nginx/html

# Copy nginx configuration
COPY nginx.conf /etc/nginx/nginx.conf

# Create startup script
RUN echo '#!/bin/bash\n\
set -e\n\
# Start nginx in background\n\
nginx\n\
# Start server in foreground\n\
exec /app/server\n\
' > /app/start.sh && chmod +x /app/start.sh

EXPOSE 80

CMD ["/app/start.sh"]

