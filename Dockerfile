# ---- 前端构建（Vue 控制台）----
FROM node:22-bookworm AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# ---- Rust 网关编译 ----
FROM rust:1.97-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends gcc pkg-config cmake perl \
    && rm -rf /var/lib/apt/lists/*
# 国内 crates.io 镜像（中科大 USTC sparse，实测全依赖 16s 拉完）
RUN mkdir -p "$CARGO_HOME" && cat > "$CARGO_HOME/config.toml" <<'EOF'
[source.crates-io]
replace-with = "ustc-sparse"

[source.ustc-sparse]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"
EOF
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release

# ---- 运行镜像 ----
FROM debian:bookworm-slim
# 时区：chrono::Local（Coding Plan 时段窗判定、价格生效日默认值）依赖 TZ +
# zoneinfo 文件；默认 UTC 会让「每日 08:00-20:00 生效」类配置偏 8 小时
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && ln -snf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime \
    && echo "Asia/Shanghai" > /etc/timezone \
    && rm -rf /var/lib/apt/lists/*
ENV TZ=Asia/Shanghai
COPY --from=builder /app/target/release/llm_gateway /usr/local/bin/llm_gateway
COPY --from=web /web/dist /app/web/dist
ENV GATEWAY_WEB_DIR=/app/web/dist
EXPOSE 443 80
ENTRYPOINT ["llm_gateway"]
