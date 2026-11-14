#!/usr/bin/env bash
# 本机编译 release 并部署 docker（绕过容器内慢编译）
# 运行时镜像 Dockerfile.runtime = ubuntu:24.04（glibc 2.39，与本机匹配），仅打包产物，构建秒级
# 用法: bash scripts/deploy-local-binary.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/llm_gateway

echo "==> [1/4] 本机编译 release（16 核，利用本地依赖缓存）..."
cargo build --release
[ -x "$BIN" ] || { echo "错误: 编译失败，$BIN 不存在"; exit 1; }

# 部署前数据库快照（尽力而为，失败不阻断）：回滚二进制撞上 VersionMissing 时需要快照恢复
echo "==> [2/4] 部署前数据库快照..."
mkdir -p backups
docker exec llm_gateway-postgres-1 pg_dump -U gateway llm_gateway > "backups/pre-deploy-$(date +%Y%m%d-%H%M%S).sql" \
  || echo "警告: pg_dump 失败，继续部署"

echo "==> [3/4] 构建运行时镜像（仅 COPY 产物，秒级）..."
docker-compose build gateway

# 非 root 容器（组 65532）需组读 TLS 私钥：key 为 600 时容器读不到；宿主机 chgrp 失败时经容器 root 兜底，兜底再失败则中止部署
if [ -f certs/key.pem ]; then
    chgrp 65532 certs/key.pem 2>/dev/null || {
        echo "==> 宿主机 chgrp 失败，经 docker 容器以 root 修复 key 属组…"
        docker run --rm -v "$(pwd)/certs:/certs" ubuntu:24.04 chgrp 65532 /certs/key.pem
        docker run --rm -v "$(pwd)/certs:/certs" ubuntu:24.04 chmod 640 /certs/key.pem
    }
    chmod 640 certs/key.pem 2>/dev/null || true
fi

echo "==> [4/4] 启动/更新网关容器..."
docker-compose up -d gateway

echo "==> 完成。"
echo "    验证: docker-compose logs --tail=20 gateway"
echo "    验证: curl -sk https://127.0.0.1:8443/api/auth/login ..."
