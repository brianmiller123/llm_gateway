#!/usr/bin/env bash
# 向 OpenLDAP 冒烟实例导入种子数据（alice/bob/admins 组）。
# 用法：docker-compose --profile ldap up -d ldap && bash scripts/seed-ldap.sh
# 幂等：已存在的条目跳过（ldapadd 报 Already exists 属正常）。
set -euo pipefail

CONTAINER="llm_gateway-ldap-1"
ADMIN_DN="cn=admin,dc=example,dc=org"
ADMIN_PW="${LDAP_ADMIN_PASSWORD:-adminpw}"

docker cp scripts/ldap-seed/50-seed.ldif "$CONTAINER":/tmp/seed.ldif
docker exec "$CONTAINER" ldapadd -x -H ldap://localhost -D "$ADMIN_DN" -w "$ADMIN_PW" -f /tmp/seed.ldif || true
echo "seeded LDAP users: alice/alicepw, bob/bobpw (group: admins)"
