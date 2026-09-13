# 阶段1：编译 Rust 运维二进制
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# 阶段2：运行时（纯无头，浏览器用 Debian 官方 chromium，无需 Node/Playwright）
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates chromium \
      fonts-wqy-zenhei fonts-noto-cjk \
      libnss3 libnspr4 libgbm1 libasound2 libx11-6 libx11-xcb1 \
      libxcomposite1 libxdamage1 libxext6 libxfixes3 libxrandr2 \
      libpangocairo-1.0-0 libcairo2 libcups2 libdbus-1-3 \
      libatk1.0-0 libatk-bridge2.0-0 libgtk-3-0 libatspi2.0-0 libxshmfence1 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/sub2api-operator /usr/local/bin/
# config.toml / credentials.db 运行时挂载进 /app，不进镜像
CMD ["sub2api-operator"]
