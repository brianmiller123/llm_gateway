#!/usr/bin/env bash
# 本机编译 release 并部署 docker（绕过容器内慢编译）
# 运行时镜像 Dockerfile.runtime = ubuntu:24.04（glibc 2.39，与本机匹配），仅打包产物，构建秒级
# 用法: bash scripts/deploy-local-binary.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/llm_gateway

echo "==> [1/3] 本机编译 release（16 核，利用本地依赖缓存）..."
cargo build --release
[ -x "$BIN" ] || { echo "错误: 编译失败，$BIN 不存在"; exit 1; }

echo "==> [2/3] 构建运行时镜像（仅 COPY 产物，秒级）..."
docker-compose build gateway

echo "==> [3/3] 启动/更新网关容器..."
docker-compose up -d gateway

echo "==> 完成。"
echo "    验证: docker-compose logs --tail=20 gateway"
echo "    验证: curl -sk https://127.0.0.1:8443/api/auth/login ..."
