# 多阶段构建：前端静态导出（Node 仅此阶段）→ Rust 编译（rustls 静态链接）→ 极小运行镜像。
# 配合 docker buildx --platform linux/amd64,linux/arm64,linux/arm/v7 可一次产出多架构镜像。
FROM node:22-alpine AS web
WORKDIR /app/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
# output: 'export' 与 Route Handlers 不能共存，构建期临时移出
RUN mv src/app/api /tmp/api \
    && NEXT_TELEMETRY_DISABLED=1 npx next build \
    && mv /tmp/api src/app/api \
    && cp -a out /dist

FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ ./src/
COPY --from=web /dist ./web/dist
RUN cargo build --release --locked

FROM alpine:3.21
RUN apk add --no-cache ca-certificates tzdata
COPY --from=builder /app/target/release/libretv-rs /usr/local/bin/libretv-rs
ENV HOST=0.0.0.0 PORT=8080 DATA_DIR=/data
# 服务端持久化数据目录（SQLite）；非 root 用户需要可写
RUN mkdir -p /data && chown 65532:65532 /data
VOLUME /data
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/libretv-rs"]
