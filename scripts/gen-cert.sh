#!/usr/bin/env bash
# 生成自签证书（内部部署；公网部署请改用 ACME/公司 CA）
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p certs
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout certs/key.pem -out certs/cert.pem -days 365 -nodes \
  -subj "/CN=llm-gateway" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
chmod 600 certs/key.pem
echo "OK: certs/cert.pem + certs/key.pem generated"
