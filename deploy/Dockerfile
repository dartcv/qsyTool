# syntax=docker/dockerfile:1.7

FROM node:22-bookworm-slim AS web-builder
WORKDIR /workspace
COPY package.json package-lock.json ./
RUN npm ci
COPY index.html tsconfig*.json vite.config.ts ./
COPY public ./public
COPY src ./src
RUN npm run build

FROM rust:1.96-bookworm AS api-builder
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
RUN cargo build --locked --release --package qsy-resolver-api

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 qsy \
    && useradd --uid 10001 --gid qsy --create-home --shell /usr/sbin/nologin qsy \
    && mkdir -p /app/data/downloads \
    && chown -R qsy:qsy /app

WORKDIR /app
COPY --from=api-builder /workspace/target/release/qsy-resolver-api /usr/local/bin/qsy-resolver-api
COPY --from=web-builder /workspace/dist ./dist

ENV QSY_API_BIND_ADDR=0.0.0.0:8080 \
    QSY_API_DOWNLOAD_DIR=/app/data/downloads \
    QSY_WEB_DIR=/app/dist \
    QSY_API_CORS_ORIGINS=* \
    QSY_API_MAX_IN_FLIGHT=8 \
    QSY_API_MAX_DOWNLOAD_BYTES=2147483648 \
    QSY_API_LOG_FORMAT=json \
    RUST_LOG=qsy_resolver_api=info,tower_http=info

EXPOSE 8080
VOLUME ["/app/data"]
USER qsy:qsy
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 CMD curl --fail --silent --show-error http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/qsy-resolver-api"]
