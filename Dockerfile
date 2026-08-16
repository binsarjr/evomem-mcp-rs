# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1.95 AS builder
WORKDIR /app

# Fetch dependencies first (cached layer) so src changes rebuild fast.
COPY Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo fetch

COPY src ./src
RUN cargo build --release

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/evomem-mcp-rs /usr/local/bin/evomem-mcp-rs

ENV EVOMEM_ROOT=/vault \
    EVOMEM_DEFAULT_NAMESPACE=default \
    BIND=0.0.0.0:8080

EXPOSE 8080
VOLUME ["/vault"]

ENTRYPOINT ["evomem-mcp-rs"]
