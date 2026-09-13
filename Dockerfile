# 阶段1：编译 Rust 运维二进制
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# 阶段2：安装 playwright chromium（与 xiic-crm 同构，装到 /ms-playwright）
FROM node:22-bookworm-slim AS pw
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
WORKDIR /pw
COPY browser-worker/package.json ./
RUN npm install && npx playwright-core install chromium

# 阶段3：运行时（纯无头，不需要虚拟桌面/Xvfb）
FROM node:22-bookworm-slim AS runtime
ENV PLAYWRIGHT_BROWSERS_PATH=/ms-playwright
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      fonts-wqy-zenhei fonts-noto-cjk \
      libnss3 libnspr4 libgbm1 libasound2 libx11-6 libx11-xcb1 \
      libxcomposite1 libxdamage1 libxext6 libxfixes3 libxrandr2 \
      libpangocairo-1.0-0 libcairo2 libcups2 libdbus-1-3 \
      libatk1.0-0 libatk-bridge2.0-0 libgtk-3-0 libatspi2.0-0 libxshmfence1 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=pw /ms-playwright /ms-playwright
COPY --from=pw /pw/node_modules /app/browser-worker/node_modules
COPY browser-worker/worker.js /app/browser-worker/worker.js
COPY --from=builder /app/target/release/sub2api-operator /usr/local/bin/
# config.toml / credentials.db 运行时挂载进 /app，不进镜像
CMD ["sub2api-operator"]
