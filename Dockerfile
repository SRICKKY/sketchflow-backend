# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked --jobs 2 && \
    cp target/release/sketchflow-backend /usr/local/bin/sketchflow-backend

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 sketchflow && \
    useradd --uid 10001 --gid sketchflow --no-create-home sketchflow && \
    mkdir -p /app/uploads && \
    chown sketchflow:sketchflow /app/uploads

WORKDIR /app
COPY --from=builder /usr/local/bin/sketchflow-backend /usr/local/bin/sketchflow-backend
USER sketchflow
ENV HOST=0.0.0.0 PORT=8080 ASSET_STORAGE_DIR=/app/uploads
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8080/health || exit 1
CMD ["sketchflow-backend"]
